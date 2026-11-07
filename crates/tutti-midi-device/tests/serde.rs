//! A saved id reads back as the id it was, and the document's shape is the one
//! apps persist. Run with `--features serde`.
//!
//! Mutation record (planted, seen to fail, restored):
//! - `serde(skip)` on `DeviceId`'s ordinal: `an_id_round_trips` fails (every
//!   ordinal reads back 0), and so does the pinned text;
//! - `serde(rename = "p")` on the `Alsa` variant's `port`: the pinned text fails;
//! - a variant reordered in `Identity`: nothing fails, as JSON names the
//!   variant (an equivalent mutant).
#![cfg(feature = "serde")]

use tutti_midi_device::{
    DeviceEvent, DeviceId, DeviceTable, Direction, EndpointId, Identity, OsChange, OsDevice,
    Resolved,
};

fn listed(identity: Identity, count: u64) -> (DeviceTable, Vec<DeviceId>) {
    let mut table = DeviceTable::default();
    let ids = (0..count)
        .map(|n| {
            let Some(DeviceEvent::Added(info)) = table.apply(OsChange::Present(OsDevice {
                handle: EndpointId::from_raw(n),
                identity: identity.clone(),
                name: "Keys".into(),
                direction: Direction::Input,
            })) else {
                unreachable!("a new handle is added")
            };
            info.id
        })
        .collect();
    (table, ids)
}

fn round_trip(id: &DeviceId) -> DeviceId {
    serde_json::from_str(&serde_json::to_string(id).unwrap()).unwrap()
}

#[test]
fn an_id_round_trips() {
    for identity in [
        Identity::Product("abc".into()),
        Identity::CoreMidi(-7),
        Identity::alsa("Keystep", "MIDI 1"),
    ] {
        let (_, ids) = listed(identity, 3);
        for id in ids {
            assert_eq!(round_trip(&id), id);
        }
    }
}

#[test]
fn the_document_names_the_identity_and_the_ordinal() {
    let (_, ids) = listed(Identity::alsa("Keystep", "MIDI 1"), 2);
    assert_eq!(
        serde_json::to_string(&ids[1]).unwrap(),
        r#"{"identity":{"Alsa":{"client":"Keystep","port":"MIDI 1"}},"ordinal":1}"#
    );
}

/// What an app does with a saved id: read it, resolve it, and bind only an
/// exact answer.
#[test]
fn a_restored_id_binds_only_when_exact() {
    let (table, ids) = listed(Identity::Product("abc".into()), 1);
    assert!(matches!(table.resolve(&round_trip(&ids[0])), Resolved::Exact(d) if d.id == ids[0]));

    let (twins, ids) = listed(Identity::Product("abc".into()), 2);
    assert!(matches!(twins.resolve(&round_trip(&ids[0])), Resolved::Ambiguous(c) if c.len() == 2));
}
