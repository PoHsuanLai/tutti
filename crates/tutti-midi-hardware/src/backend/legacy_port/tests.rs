use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::anchor::Anchor;
use super::bytes::{ByteIn, ByteOut, Wire};
use super::poll;
use super::ports::{PortInfo, Side, endpoint, endpoint_id, endpoints, locate, slot_of};
use super::schedule::Schedule;
use crate::Devices;
use crate::capability::{EndpointInfo, UmpCapability};
use crate::devices::ChangeSink;
use crate::endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
use crate::error::{Error, Result};
use tutti_midi_device::{DeviceEvent, EndpointId, Identity, OsDevice, PortDirection, WinMmDevice};
use tutti_ump::{Message, MidiGroup, Sysex7Packet, SysexStatus, Ump};

fn named(name: &str, product: u16) -> WinMmDevice {
    WinMmDevice::Named {
        name: name.to_owned(),
        manufacturer: 0x41,
        product,
    }
}

fn info(name: &str) -> PortInfo {
    PortInfo {
        name: name.to_owned(),
        device: named(name, 7),
    }
}

// ---- ports ----------------------------------------------------------------

#[test]
fn an_id_gives_back_its_side_and_position() {
    for (side, index) in [(Side::Input, 0), (Side::Output, 5), (Side::Input, u32::MAX)] {
        let id = endpoint_id(side, index, &info("Keystep"));
        let slot = slot_of(id);
        assert_eq!((slot.side, slot.index), (side, index));
        assert!(slot.matches(&info("Keystep")));
    }
}

#[test]
fn an_id_refuses_the_port_that_took_its_position() {
    // The list shifts when a device is unplugged: position 1 is another device.
    let slot = slot_of(endpoint_id(Side::Input, 1, &info("Keystep")));
    assert!(!slot.matches(&info("Launchpad")));
    let other_product = PortInfo {
        device: named("Keystep", 8),
        ..info("Keystep")
    };
    assert!(!slot.matches(&other_product));
}

#[test]
fn an_interface_path_names_the_device_whatever_its_position_or_name() {
    let path = |name: &str| PortInfo {
        name: name.to_owned(),
        device: WinMmDevice::Interface("\\\\?\\usb#vid_0001".to_owned()),
    };
    let slot = slot_of(endpoint_id(Side::Input, 3, &path("Keystep")));
    // The same device, listed elsewhere with a different cut of its name.
    assert!(slot.matches(&path("Keystep Pro MIDI")));
    let other = PortInfo {
        device: WinMmDevice::Interface("\\\\?\\usb#vid_0002".to_owned()),
        ..path("Keystep")
    };
    assert!(!slot.matches(&other));
}

#[test]
fn a_device_that_moved_in_the_list_is_found_where_it_went_and_one_gone_is_not() {
    let slot = slot_of(endpoint_id(Side::Input, 2, &info("B")));
    let list = |names: &[&str]| -> Vec<(u32, PortInfo)> {
        names.iter().zip(0..).map(|(n, i)| (i, info(n))).collect()
    };
    assert_eq!(
        locate(slot, list(&["A", "C", "B"])),
        Some(2),
        "where it was"
    );
    assert_eq!(
        locate(slot, list(&["B", "C"])),
        Some(0),
        "an earlier device left"
    );
    assert_eq!(locate(slot, list(&["A", "C"])), None, "unplugged");
}

#[test]
fn the_input_and_the_output_of_one_unit_have_different_ids_and_identities() {
    let input = endpoint(Side::Input, 0, info("USB MIDI"));
    let output = endpoint(Side::Output, 0, info("USB MIDI"));
    assert_ne!(input.id, output.id);
    assert_ne!(input.identity, output.identity);
    assert_eq!(input.identity, Identity::winmm_input(named("USB MIDI", 7)));
    assert_eq!(
        output.identity,
        Identity::winmm_output(named("USB MIDI", 7))
    );
}

#[test]
fn a_byte_port_is_listed_as_midi1_with_no_blocks_in_list_order() {
    let listed = endpoints(Side::Output, vec![(0, info("A")), (1, info("B"))]);
    assert_eq!(
        listed.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
        ["A", "B"]
    );
    assert_eq!(slot_of(listed[1].id).index, 1);
    assert!(
        listed
            .iter()
            .all(|e| e.capability == UmpCapability::midi1())
    );
}

// ---- bytes in -------------------------------------------------------------

#[test]
fn a_packed_note_on_is_the_midi1_ump_of_its_bytes() {
    // 0x90 0x3C 0x64, low byte first: MIDI 1.0 Channel Voice in type 2 (M2-104 7.3).
    let ump = ByteIn::new().short(0x0064_3C90).unwrap();
    assert_eq!(ump.0, [0x2090_3C64, 0, 0, 0]);
}

#[test]
fn a_packed_program_change_has_one_data_byte_and_ignores_the_padding() {
    let ump = ByteIn::new().short(0x7F00_0AC3).unwrap();
    assert_eq!(ump.0[0], 0x20C3_0A00);
}

#[test]
fn a_packed_real_time_status_is_a_system_message() {
    assert_eq!(ByteIn::new().short(0xF8).unwrap().0[0], 0x10F8_0000);
}

#[test]
fn a_packed_data_byte_with_no_status_is_nothing() {
    assert_eq!(ByteIn::new().short(0x0000_0040), None);
}

#[test]
fn a_sysex_split_across_two_buffers_is_one_sysex7_packet() {
    let mut input = ByteIn::new();
    assert_eq!(input.long(&[0xF0, 0x7E, 0x7F]).count(), 0, "not whole yet");
    let packets: Vec<Ump> = input.long(&[0x06, 0x01, 0xF7]).collect();
    // Complete (status 0), four data bytes (M2-104 7.7).
    assert_eq!(packets, [Ump([0x3004_7E7F, 0x0601_0000, 0, 0])]);
}

#[test]
fn a_short_message_does_not_end_a_sysex_in_progress() {
    let mut input = ByteIn::new();
    assert_eq!(input.long(&[0xF0, 0x01]).count(), 0);
    assert!(input.short(0xF8).is_some());
    let packets: Vec<Ump> = input.long(&[0xF7]).collect();
    assert_eq!(packets, [Ump([0x3001_0100, 0, 0, 0])]);
}

// ---- bytes out ------------------------------------------------------------

fn sent(out: &mut ByteOut, ump: Ump) -> Vec<Vec<u8>> {
    let mut wire: Vec<Vec<u8>> = Vec::new();
    out.encode::<()>(ump, |w| {
        wire.push(match w {
            Wire::Short(m) => m.to_le_bytes().to_vec(),
            Wire::Long(b) => b.to_vec(),
        });
        Ok(())
    })
    .unwrap();
    wire
}

fn sysex(status: SysexStatus, data: &[u8]) -> Ump {
    Ump::from(Message::Sysex7(
        MidiGroup::FIRST,
        Sysex7Packet::new(status, data).unwrap(),
    ))
}

#[test]
fn a_midi1_note_on_is_one_packed_short_message() {
    let note_on = Ump([0x2090_3C64, 0, 0, 0]);
    assert_eq!(
        sent(&mut ByteOut::new(), note_on),
        [vec![0x90, 0x3C, 0x64, 0]]
    );
}

#[test]
fn a_midi1_program_change_packs_two_bytes() {
    let program = Ump([0x20C3_0A00, 0, 0, 0]);
    assert_eq!(sent(&mut ByteOut::new(), program), [vec![0xC3, 0x0A, 0, 0]]);
}

#[test]
fn sysex7_packets_are_sent_as_one_framed_message_at_the_end_packet() {
    let mut out = ByteOut::new();
    assert!(sent(&mut out, sysex(SysexStatus::Start, &[1, 2, 3, 4, 5, 6])).is_empty());
    assert!(
        sent(
            &mut out,
            sysex(SysexStatus::Continue, &[7, 8, 9, 10, 11, 12])
        )
        .is_empty()
    );
    let message = sent(&mut out, sysex(SysexStatus::End, &[13]));
    let mut want = vec![0xF0];
    want.extend(1..=13);
    want.push(0xF7);
    assert_eq!(message, [want]);
}

#[test]
fn a_complete_sysex7_packet_is_a_whole_message() {
    let message = sent(
        &mut ByteOut::new(),
        sysex(SysexStatus::Complete, &[0x7E, 0x7F]),
    );
    assert_eq!(message, [vec![0xF0, 0x7E, 0x7F, 0xF7]]);
}

#[test]
fn a_message_with_no_midi1_form_sends_nothing() {
    // Utility: a NOOP (type 0).
    assert!(sent(&mut ByteOut::new(), Ump::default()).is_empty());
}

#[test]
fn a_failed_send_stops_the_message_and_is_returned() {
    let mut out = ByteOut::new();
    let failed = out.encode(Ump([0x2090_3C64, 0, 0, 0]), |_| Err("no"));
    assert_eq!(failed, Err("no"));
}

// ---- schedule -------------------------------------------------------------

#[test]
fn a_schedule_releases_the_earliest_first_and_ties_in_push_order() {
    let t0 = Instant::now();
    let at = |ms| t0 + Duration::from_millis(ms);
    let mut held = Schedule::new();
    held.push(at(30), "late");
    held.push(at(10), "first");
    held.push(at(10), "second");
    held.push(at(20), "middle");
    assert_eq!(held.len(), 4);
    let now = at(25);
    let due: Vec<_> = std::iter::from_fn(|| held.pop_due(now)).collect();
    assert_eq!(due, ["first", "second", "middle"]);
    assert_eq!(held.pop_due(now), None, "the last is still to wait");
}

#[test]
fn a_schedule_says_how_long_until_the_earliest_and_zero_when_late() {
    let t0 = Instant::now();
    let mut held = Schedule::new();
    assert_eq!(held.wait(t0), None);
    held.push(t0 + Duration::from_millis(40), ());
    assert_eq!(
        held.wait(t0 + Duration::from_millis(15)),
        Some(Duration::from_millis(25))
    );
    assert_eq!(
        held.wait(t0 + Duration::from_millis(90)),
        Some(Duration::ZERO)
    );
}

// ---- anchor ---------------------------------------------------------------

#[test]
fn a_stamp_is_that_long_after_the_start() {
    let start = Instant::now();
    let mut anchor = Anchor::new(start);
    let at = anchor.arrival(250, start + Duration::from_millis(260));
    assert_eq!(at, start + Duration::from_millis(250));
}

#[test]
fn a_stamp_seen_before_it_is_due_arrives_when_it_was_seen_and_moves_the_start() {
    let start = Instant::now();
    let mut anchor = Anchor::new(start);
    let now = start + Duration::from_millis(100);
    assert_eq!(anchor.arrival(500, now), now);
    // The next event, 100 ms after that one by the stamps, is placed from the
    // moved start: seen 100 ms later, it arrived when it was seen.
    let later = now + Duration::from_millis(100);
    assert_eq!(anchor.arrival(600, later), later);
}

#[test]
fn a_stamp_more_than_a_second_old_re_anchors_to_now() {
    let start = Instant::now();
    let mut anchor = Anchor::new(start);
    let now = start + Duration::from_secs(5);
    assert_eq!(anchor.arrival(100, now), now);
    let next = now + Duration::from_millis(40);
    assert_eq!(anchor.arrival(140, next), next);
}

// ---- the poll watch -------------------------------------------------------

static LISTED: Mutex<Vec<OsDevice>> = Mutex::new(Vec::new());

fn device(n: u64, name: &str) -> OsDevice {
    OsDevice {
        handle: EndpointId::from_raw(n),
        identity: Identity::winmm_input(named(name, 1)),
        name: name.to_owned(),
        direction: PortDirection::Input,
    }
}

static LOOKS: AtomicUsize = AtomicUsize::new(0);

fn listed() -> Vec<OsDevice> {
    LOOKS.fetch_add(1, Ordering::SeqCst);
    LISTED.lock().unwrap().clone()
}

struct Polled;

impl MidiEndpoints for Polled {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        Ok(Box::new(poll::start(
            sink,
            listed,
            Duration::from_millis(5),
        )?))
    }
    fn inputs(&self) -> Vec<EndpointInfo> {
        Vec::new()
    }
    fn outputs(&self) -> Vec<EndpointInfo> {
        Vec::new()
    }
    fn open_input(
        &self,
        _: EndpointId,
        _: crate::InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        Err(Error::Unsupported("test"))
    }
    fn open_output(&self, _: EndpointId) -> Result<Box<dyn DeviceOut>> {
        Err(Error::Unsupported("test"))
    }
}

/// One test owns `LISTED`, so the process-wide list is not raced.
#[test]
fn the_poll_seeds_then_reports_a_device_that_appears_and_one_that_goes() {
    *LISTED.lock().unwrap() = vec![device(1, "A")];
    let devices = Devices::open_over(Box::new(Polled)).unwrap();
    assert_eq!(devices.list().len(), 1, "seeded before open returns");
    let events = devices.subscribe();

    LISTED.lock().unwrap().push(device(2, "B"));
    let Ok(DeviceEvent::Added(added)) = events.recv_timeout(Duration::from_secs(5)) else {
        panic!("B was not reported");
    };
    assert_eq!(added.name, "B");

    LISTED.lock().unwrap().remove(0);
    let Ok(DeviceEvent::Removed(removed)) = events.recv_timeout(Duration::from_secs(5)) else {
        panic!("A was not reported gone");
    };
    assert_eq!(removed.name, "A");

    // Once the last holder drops, the thread has ended: no look follows.
    drop(events);
    drop(devices);
    let looks = LOOKS.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(LOOKS.load(Ordering::SeqCst), looks);
}
