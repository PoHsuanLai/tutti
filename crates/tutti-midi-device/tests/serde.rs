//! A saved device reads back as the device it was, and the document's shape is
//! the one apps persist: renaming a variant or a field breaks saved files, so
//! the text is pinned for every `Identity`. Run with `--features serde`.
#![cfg(feature = "serde")]

use tutti_midi_device::{
    DeviceEvent, DeviceInfo, DeviceTable, EndpointId, Identity, OsChange, OsDevice, PortDirection,
    Resolved,
};

fn listed(identity: Identity, count: u64) -> (DeviceTable, Vec<DeviceInfo>) {
    let mut table = DeviceTable::default();
    let infos = (0..count)
        .map(|n| {
            let Some(DeviceEvent::Added(info)) = table.apply(OsChange::Present(OsDevice {
                handle: EndpointId::from_raw(n),
                identity: identity.clone(),
                name: "Keys".into(),
                direction: PortDirection::Input,
            })) else {
                unreachable!("a new handle is added")
            };
            info
        })
        .collect();
    (table, infos)
}

fn round_trip(info: &DeviceInfo) -> DeviceInfo {
    serde_json::from_str(&serde_json::to_string(info).unwrap()).unwrap()
}

#[test]
fn a_device_round_trips() {
    for identity in [
        Identity::Product("abc".into()),
        Identity::CoreMidi(-7),
        Identity::alsa("Keystep", "MIDI 1"),
    ] {
        let (_, infos) = listed(identity, 3);
        for info in infos {
            assert_eq!(round_trip(&info), info);
        }
    }
}

#[test]
fn the_document_names_each_identity_and_the_ordinal() {
    let pinned = [
        (
            Identity::alsa("Keystep", "MIDI 1"),
            r#"{"identity":{"Alsa":{"client":"Keystep","port":"MIDI 1"}},"ordinal":1}"#,
        ),
        (
            Identity::Product("abc".into()),
            r#"{"identity":{"Product":"abc"},"ordinal":1}"#,
        ),
        (
            Identity::CoreMidi(-7),
            r#"{"identity":{"CoreMidi":-7},"ordinal":1}"#,
        ),
    ];
    for (identity, text) in pinned {
        let (_, infos) = listed(identity, 2);
        assert_eq!(serde_json::to_string(&infos[1].id).unwrap(), text);
    }
}

#[test]
fn the_document_names_the_direction() {
    let (_, infos) = listed(Identity::CoreMidi(1), 1);
    assert_eq!(
        serde_json::to_string(&infos[0]).unwrap(),
        r#"{"id":{"identity":{"CoreMidi":1},"ordinal":0},"name":"Keys","direction":"Input"}"#
    );
}

/// What an app does with a saved device: read it, resolve it, and bind only an
/// exact answer.
#[test]
fn a_restored_device_binds_only_when_exact() {
    let (table, infos) = listed(Identity::CoreMidi(1), 1);
    assert_eq!(
        table.resolve(&round_trip(&infos[0])),
        Resolved::Exact(infos[0].clone())
    );

    let (named, infos) = listed(Identity::alsa("Keystep", "MIDI 1"), 1);
    assert_eq!(
        named.resolve(&round_trip(&infos[0])),
        Resolved::SameName(infos[0].clone())
    );

    let (twins, infos) = listed(Identity::alsa("Keystep", "MIDI 1"), 2);
    assert!(matches!(
        twins.resolve(&round_trip(&infos[0])),
        Resolved::Ambiguous(c) if c.len() == 2
    ));
}
