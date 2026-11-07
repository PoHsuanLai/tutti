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
    // Mutation record, each seen to fail, then restored:
    // - `present` returns `Added` for a known handle (skips the by_handle
    //   lookup): `added_and_removed_alternate_per_id` fails;
    // - `gone` keeps the `by_handle` entry: `list_agrees_with_the_os` fails;
    // - the ordinal search starts at 1: `two_identical_devices_get_distinct_ordinals`
    //   fails;
    // - `present` skips the `Changed` for a rename: `list_agrees_with_the_os`
    //   fails on the name;
    // - `present` overwrites the id on a rename: `a_rename_is_changed_and_keeps_the_id`
    //   fails.
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
