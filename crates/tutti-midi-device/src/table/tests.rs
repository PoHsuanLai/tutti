use std::collections::{BTreeMap, HashMap};

use proptest::prelude::*;

use super::*;

fn handle(n: u64) -> EndpointId {
    EndpointId::from_raw(n)
}

fn dev(n: u64, client: &str, port: &str, direction: Direction) -> OsDevice {
    OsDevice {
        handle: handle(n),
        identity: Identity::alsa(client, port),
        name: format!("{client}: {port}"),
        direction,
    }
}

fn keys(n: u64) -> OsDevice {
    dev(n, "Keystep", "MIDI 1", Direction::Input)
}

fn added(event: Option<DeviceEvent>) -> DeviceInfo {
    match event {
        Some(DeviceEvent::Added(info)) => info,
        other => panic!("expected Added, got {other:?}"),
    }
}

#[test]
fn a_new_handle_is_added_and_listed() {
    let mut table = DeviceTable::default();
    let info = added(table.apply(OsChange::Present(keys(1))));
    assert_eq!(info.name, "Keystep: MIDI 1");
    assert_eq!(info.direction, Direction::Input);
    assert_eq!(table.list(), std::slice::from_ref(&info));
    assert_eq!(table.get(&info.id), Some(&info));
    assert_eq!(table.handle(&info.id), Some(handle(1)));
}

#[test]
fn a_repeat_of_a_known_state_is_none() {
    let mut table = DeviceTable::default();
    table.apply(OsChange::Present(keys(1)));
    assert_eq!(table.apply(OsChange::Present(keys(1))), None);
    assert_eq!(table.list().len(), 1);
}

#[test]
fn gone_returns_the_last_info_and_unlists() {
    let mut table = DeviceTable::default();
    let info = added(table.apply(OsChange::Present(keys(1))));
    assert_eq!(
        table.apply(OsChange::Gone(handle(1))),
        Some(DeviceEvent::Removed(info.clone()))
    );
    assert!(table.list().is_empty());
    assert_eq!(table.handle(&info.id), None);
}

#[test]
fn the_departure_of_an_unknown_handle_is_none() {
    let mut table = DeviceTable::default();
    assert_eq!(table.apply(OsChange::Gone(handle(9))), None);
    table.apply(OsChange::Present(keys(1)));
    table.apply(OsChange::Gone(handle(1)));
    assert_eq!(table.apply(OsChange::Gone(handle(1))), None);
}

#[test]
fn a_rename_is_changed_and_keeps_the_id() {
    let mut table = DeviceTable::default();
    let info = added(table.apply(OsChange::Present(keys(1))));
    let renamed = OsDevice {
        name: "Keystep Pro".into(),
        ..keys(1)
    };
    let Some(DeviceEvent::Changed(now)) = table.apply(OsChange::Present(renamed)) else {
        panic!("a rename is Changed");
    };
    assert_eq!(now.id, info.id);
    assert_eq!(now.name, "Keystep Pro");
    assert_eq!(table.list(), [now]);
}

#[test]
fn a_direction_change_is_changed() {
    let mut table = DeviceTable::default();
    table.apply(OsChange::Present(keys(1)));
    let duplex = OsDevice {
        direction: Direction::Duplex,
        ..keys(1)
    };
    let Some(DeviceEvent::Changed(now)) = table.apply(OsChange::Present(duplex)) else {
        panic!("a direction change is Changed");
    };
    assert!(now.direction.is_input() && now.direction.is_output());
}

#[test]
fn two_identical_devices_get_distinct_ordinals() {
    let mut table = DeviceTable::default();
    let a = added(table.apply(OsChange::Present(keys(1))));
    let b = added(table.apply(OsChange::Present(keys(2))));
    assert_eq!((a.id.ordinal(), b.id.ordinal()), (0, 1));
    assert_eq!(a.id.identity(), b.id.identity());
    assert_ne!(a.id, b.id);
    assert_eq!(table.handle(&a.id), Some(handle(1)));
    assert_eq!(table.handle(&b.id), Some(handle(2)));
}

#[test]
fn a_freed_ordinal_is_taken_by_the_next_identical_device() {
    let mut table = DeviceTable::default();
    let a = added(table.apply(OsChange::Present(keys(1))));
    added(table.apply(OsChange::Present(keys(2))));
    table.apply(OsChange::Gone(handle(1)));
    // The OS reuses the client number for the same keyboard plugged back in.
    let again = added(table.apply(OsChange::Present(keys(3))));
    assert_eq!(again.id, a.id);
}

#[test]
fn a_re_enumeration_changes_nothing() {
    let mut table = DeviceTable::default();
    let all = [keys(1), keys(2), dev(3, "Synth", "Out", Direction::Output)];
    for d in &all {
        table.apply(OsChange::Present(d.clone()));
    }
    let before = table.list();
    for d in all.iter().rev() {
        assert_eq!(table.apply(OsChange::Present(d.clone())), None);
    }
    assert_eq!(table.list(), before);
}

#[test]
fn a_renamed_device_does_not_collide_with_a_new_one_of_the_old_name() {
    let mut table = DeviceTable::default();
    let a = added(table.apply(OsChange::Present(keys(1))));
    table.apply(OsChange::Present(OsDevice {
        name: "Renamed".into(),
        ..keys(1)
    }));
    // A new device with the old identity takes ordinal 1: the renamed device
    // still holds `a.id`.
    let b = added(table.apply(OsChange::Present(keys(2))));
    assert_ne!(a.id, b.id);
    assert_eq!(table.list().len(), 2);
}

#[test]
fn a_product_id_is_shared_by_ports_of_one_device() {
    let mut table = DeviceTable::default();
    let port = |n| OsDevice {
        handle: handle(n),
        identity: Identity::Product("sn-0042".into()),
        name: format!("Port {n}"),
        direction: Direction::Duplex,
    };
    let a = added(table.apply(OsChange::Present(port(1))));
    let b = added(table.apply(OsChange::Present(port(2))));
    assert_eq!(a.id.identity(), b.id.identity());
    assert_ne!(a.id, b.id);
}

#[test]
fn ids_order_by_identity_then_ordinal() {
    let mut table = DeviceTable::default();
    table.apply(OsChange::Present(keys(1)));
    table.apply(OsChange::Present(keys(2)));
    table.apply(OsChange::Present(dev(3, "Another", "P", Direction::Input)));
    let names: Vec<_> = table.list().into_iter().map(|d| d.id.to_string()).collect();
    assert_eq!(
        names,
        [
            "alsa:Another/P#0",
            "alsa:Keystep/MIDI 1#0",
            "alsa:Keystep/MIDI 1#1"
        ]
    );
}

/// A small handle space and two identities, so handles are reused and
/// identities collide.
fn change() -> impl Strategy<Value = OsChange> {
    let present = (0u64..5, 0usize..2, 0usize..2, 0usize..3).prop_map(|(h, who, label, dir)| {
        OsChange::Present(OsDevice {
            handle: handle(h),
            identity: Identity::alsa(["A", "B"][who], "P"),
            name: ["one", "two"][label].to_string(),
            direction: [Direction::Input, Direction::Output, Direction::Duplex][dir],
        })
    });
    let gone = (0u64..5).prop_map(|h| OsChange::Gone(handle(h)));
    prop_oneof![3 => present, 2 => gone]
}

/// Applies `changes` to `table`, holding a present handle's identity fixed as
/// the OS does, and returns what the OS has present.
fn drive(
    table: &mut DeviceTable,
    changes: Vec<OsChange>,
    mut each: impl FnMut(Option<DeviceEvent>, &DeviceTable, &HashMap<EndpointId, OsDevice>),
) -> HashMap<EndpointId, OsDevice> {
    let mut os: HashMap<EndpointId, OsDevice> = HashMap::new();
    for change in changes {
        let change = match change {
            OsChange::Present(mut d) => {
                if let Some(old) = os.get(&d.handle) {
                    d.identity = old.identity.clone();
                }
                os.insert(d.handle, d.clone());
                OsChange::Present(d)
            }
            OsChange::Gone(h) => {
                os.remove(&h);
                OsChange::Gone(h)
            }
        };
        let event = table.apply(change);
        each(event, table, &os);
    }
    os
}

proptest! {
    #[test]
    fn added_and_removed_alternate_per_id(changes in prop::collection::vec(change(), 0..60)) {
        let mut listed: BTreeMap<DeviceId, DeviceInfo> = BTreeMap::new();
        let mut table = DeviceTable::default();
        drive(&mut table, changes, |event, _, _| match event {
            Some(DeviceEvent::Added(info)) => {
                assert!(listed.insert(info.id.clone(), info).is_none(), "Added twice");
            }
            Some(DeviceEvent::Removed(info)) => {
                assert_eq!(listed.remove(&info.id), Some(info), "Removed unlisted");
            }
            Some(DeviceEvent::Changed(info)) => {
                assert!(listed.insert(info.id.clone(), info).is_some(), "Changed unlisted");
            }
            None => {}
        });
        // The event history replays to the table's own list.
        prop_assert_eq!(table.list(), listed.into_values().collect::<Vec<_>>());
    }

    #[test]
    fn list_agrees_with_the_os(changes in prop::collection::vec(change(), 0..60)) {
        let mut table = DeviceTable::default();
        drive(&mut table, changes, |_, table, os| {
            let list = table.list();
            assert_eq!(list.len(), os.len());
            for (handle, device) in os {
                let id = list
                    .iter()
                    .map(|d| &d.id)
                    .find(|id| table.handle(id) == Some(*handle))
                    .expect("every present handle is listed");
                let info = table.get(id).unwrap();
                assert_eq!(info.name, device.name);
                assert_eq!(info.direction, device.direction);
                assert_eq!(id.identity(), &device.identity);
            }
        });
    }

    #[test]
    fn a_re_enumeration_is_silent_and_keeps_ids(changes in prop::collection::vec(change(), 0..40)) {
        let mut table = DeviceTable::default();
        let os = drive(&mut table, changes, |_, _, _| {});
        let before = table.list();
        for device in os.values() {
            prop_assert_eq!(table.apply(OsChange::Present(device.clone())), None);
        }
        prop_assert_eq!(table.list(), before);
    }
}

// resolve: a saved device against the devices present now. Every case below
// is derived from the rule in `resolve`'s doc, not from a recorded run.

fn keys_info(table: &mut DeviceTable, n: u64) -> DeviceInfo {
    added(table.apply(OsChange::Present(keys(n))))
}

fn ordinals(candidates: &[DeviceInfo]) -> Vec<u32> {
    candidates.iter().map(|d| d.id.ordinal()).collect()
}

fn port(n: u64, unit: &str, name: &str, direction: Direction) -> OsDevice {
    OsDevice {
        handle: handle(n),
        identity: Identity::Product(unit.into()),
        name: name.into(),
        direction,
    }
}

fn coremidi(n: u64, unique: i32, name: &str) -> OsDevice {
    OsDevice {
        handle: handle(n),
        identity: Identity::CoreMidi(unique),
        name: name.into(),
        direction: Direction::Input,
    }
}

#[test]
fn a_lone_device_with_the_saved_name_is_only_the_same_name() {
    let mut table = DeviceTable::default();
    let info = keys_info(&mut table, 1);
    assert_eq!(table.resolve(&info), Resolved::SameName(info));
}

#[test]
fn a_saved_zero_with_two_alike_present_is_ambiguous() {
    let mut table = DeviceTable::default();
    let first = keys_info(&mut table, 1);
    keys_info(&mut table, 2);
    let Resolved::Ambiguous(candidates) = table.resolve(&first) else {
        panic!("two alike devices are ambiguous");
    };
    assert_eq!(ordinals(&candidates), [0, 1]);
}

#[test]
fn a_saved_ordinal_above_zero_is_ambiguous_even_with_one_device_present() {
    let mut table = DeviceTable::default();
    let first = keys_info(&mut table, 1);
    let second = keys_info(&mut table, 2);
    table.apply(OsChange::Gone(handle(2)));
    assert_eq!(second.id.ordinal(), 1);
    assert_eq!(
        table.resolve(&second),
        Resolved::Ambiguous(vec![first]),
        "the lone #0 may be another unit than the saved #1"
    );
}

#[test]
fn nothing_alike_is_missing() {
    let mut table = DeviceTable::default();
    let info = DeviceInfo {
        id: DeviceId::new(Identity::alsa("Keystep", "MIDI 1"), 0),
        name: "Keystep: MIDI 1".into(),
        direction: Direction::Input,
    };
    assert_eq!(table.resolve(&info), Resolved::Missing);
    let listed = keys_info(&mut table, 1);
    table.apply(OsChange::Gone(handle(1)));
    assert_eq!(table.resolve(&listed), Resolved::Missing);
}

/// `Keystep`/`MIDI 10` sorts right after `Keystep`/`MIDI 1`, and `Keysteo` just
/// before: only the devices with the saved identity are candidates.
#[test]
fn a_neighbouring_identity_is_not_a_candidate() {
    let mut table = DeviceTable::default();
    let first = keys_info(&mut table, 1);
    keys_info(&mut table, 2);
    table.apply(OsChange::Present(dev(
        3,
        "Keystep",
        "MIDI 10",
        Direction::Input,
    )));
    table.apply(OsChange::Present(dev(
        4,
        "Keysteo",
        "MIDI 1",
        Direction::Input,
    )));
    let Resolved::Ambiguous(candidates) = table.resolve(&first) else {
        panic!("two alike");
    };
    assert_eq!(ordinals(&candidates), [0, 1]);
    assert!(candidates.iter().all(|d| d.name == "Keystep: MIDI 1"));
}

/// A saved `#0` whose twin `#0` has left resolves to the survivor, now `#1`,
/// as a same-name match: the id cannot say it was once one of two.
#[test]
fn a_saved_zero_resolves_to_the_lone_survivor_of_a_pair_by_name_only() {
    let mut table = DeviceTable::default();
    let first = keys_info(&mut table, 1);
    let second = keys_info(&mut table, 2);
    table.apply(OsChange::Gone(handle(1)));
    assert_eq!(table.resolve(&first), Resolved::SameName(second));
}

/// One unit, three ports: a Product Instance Id is shared, the ordinals are
/// 0, 1, 2 by arrival, and the saved port is the one with its name and
/// direction, whatever its ordinal is now.
#[test]
fn a_unit_port_is_told_by_name_and_direction() {
    let mut table = DeviceTable::default();
    let in1 = added(table.apply(OsChange::Present(port(1, "u1", "Pad In", Direction::Input))));
    let out1 = added(table.apply(OsChange::Present(port(
        2,
        "u1",
        "Pad Out",
        Direction::Output,
    ))));
    let in2 = added(table.apply(OsChange::Present(port(
        3,
        "u1",
        "Pad Out",
        Direction::Input,
    ))));
    assert_eq!(
        [in1.id.ordinal(), out1.id.ordinal(), in2.id.ordinal()],
        [0, 1, 2]
    );
    assert_eq!(table.resolve(&in1), Resolved::Exact(in1.clone()));
    assert_eq!(table.resolve(&out1), Resolved::Exact(out1.clone()));
    assert_eq!(table.resolve(&in2), Resolved::Exact(in2.clone()));

    // After a restart the ports arrive in another order: the saved ports are
    // still found by name and direction, not by ordinal.
    let mut after = DeviceTable::default();
    let pad_out_in = added(after.apply(OsChange::Present(port(
        9,
        "u1",
        "Pad Out",
        Direction::Input,
    ))));
    let pad_in = added(after.apply(OsChange::Present(port(8, "u1", "Pad In", Direction::Input))));
    assert_eq!(after.resolve(&in1), Resolved::Exact(pad_in));
    assert_eq!(after.resolve(&in2), Resolved::Exact(pad_out_in));
}

#[test]
fn a_unit_without_the_saved_port_is_ambiguous_over_its_ports() {
    let mut table = DeviceTable::default();
    let saved = added(table.apply(OsChange::Present(port(1, "u1", "Pad In", Direction::Input))));
    table.apply(OsChange::Gone(handle(1)));
    let other = added(table.apply(OsChange::Present(port(
        2,
        "u1",
        "Pad Out",
        Direction::Output,
    ))));
    assert_eq!(table.resolve(&saved), Resolved::Ambiguous(vec![other]));
}

/// A CoreMIDI unique id names one endpoint, so it is exact when present
/// whatever it is called now.
#[test]
fn a_coremidi_endpoint_is_exact_by_its_unique_id() {
    let mut table = DeviceTable::default();
    let saved = added(table.apply(OsChange::Present(coremidi(1, 7, "Keys"))));
    table.apply(OsChange::Gone(handle(1)));
    let back = added(table.apply(OsChange::Present(coremidi(5, 7, "Keys (renamed)"))));
    assert_eq!(table.resolve(&saved), Resolved::Exact(back));
}

#[test]
fn a_unit_with_two_identical_ports_is_ambiguous() {
    let mut table = DeviceTable::default();
    let a = added(table.apply(OsChange::Present(port(1, "u1", "Pad", Direction::Input))));
    let b = added(table.apply(OsChange::Present(port(2, "u1", "Pad", Direction::Input))));
    assert_eq!(table.resolve(&a), Resolved::Ambiguous(vec![a, b]));
}

/// A saved input whose name now belongs to an output-only device would fail to
/// open, so it is not a same-name match. Duplex still serves either way.
#[test]
fn a_lone_device_that_no_longer_goes_the_saved_way_is_ambiguous() {
    let mut table = DeviceTable::default();
    let saved = keys_info(&mut table, 1);
    table.apply(OsChange::Present(dev(
        1,
        "Keystep",
        "MIDI 1",
        Direction::Output,
    )));
    let now = table.list().remove(0);
    assert_eq!(table.resolve(&saved), Resolved::Ambiguous(vec![now]));
}

#[test]
fn a_duplex_device_still_serves_a_saved_input() {
    let mut table = DeviceTable::default();
    let saved = keys_info(&mut table, 1);
    table.apply(OsChange::Present(dev(
        1,
        "Keystep",
        "MIDI 1",
        Direction::Duplex,
    )));
    let now = table.list().remove(0);
    assert_eq!(table.resolve(&saved), Resolved::SameName(now));
}
