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

mod learned {
    use tutti_midi_device::{
        DeviceId, DeviceInfo, Learned, Match, PortDirection, ProductInstanceId, SavedDevice,
    };
    use tutti_ump::{DeviceIdentity, ManufacturerId, U7};

    fn said() -> Learned {
        Learned::new()
            .with_identity(DeviceIdentity {
                manufacturer: ManufacturerId::of(0x0042),
                family: [1, 0].map(U7::of),
                model: [7, 0].map(U7::of),
                revision: [1, 2, 3, 4].map(U7::of),
            })
            .with_instance_id(ProductInstanceId::new("SN-1").unwrap())
    }

    #[test]
    fn what_a_device_said_round_trips() {
        let json = serde_json::to_string(&said()).unwrap();
        assert_eq!(serde_json::from_str::<Learned>(&json).unwrap(), said());
        let empty = serde_json::to_string(&Learned::new()).unwrap();
        assert_eq!(
            serde_json::from_str::<Learned>(&empty).unwrap(),
            Learned::new()
        );
    }

    /// A value the type cannot hold is refused on the way in, not carried: a
    /// Product Instance Id outside printable ASCII or past 42 bytes, a
    /// manufacturer id out of range, a byte past 7 bits.
    #[test]
    fn a_document_with_a_bad_value_is_refused() {
        let good = serde_json::to_value(said()).unwrap();
        let bad = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut value = good.clone();
            edit(&mut value);
            serde_json::from_value::<Learned>(value).is_err()
        };
        assert!(bad(&|v| v["instance_id"] = "caf\u{e9}".into()));
        assert!(bad(&|v| v["instance_id"] = "x".repeat(43).into()));
        assert!(bad(&|v| v["identity"]["family"][0] = 200.into()));
        assert!(bad(&|v| v["identity"]["manufacturer"] = 70_000.into()));
        assert!(!bad(&|_| {}));
    }

    #[test]
    fn a_saved_device_round_trips_and_a_bare_info_reads_as_knowing_nothing() {
        let table_info = serde_json::json!({
            "id": { "identity": { "Alsa": { "client": "K", "port": "P" } }, "ordinal": 0 },
            "name": "K: P",
            "direction": "Input",
        });
        let info: DeviceInfo = serde_json::from_value(table_info.clone()).unwrap();
        assert_eq!(
            info.id,
            serde_json::from_value::<DeviceId>(table_info["id"].clone()).unwrap()
        );
        assert_eq!(info.direction, PortDirection::Input);

        let saved = SavedDevice::new(info, said());
        let json = serde_json::to_string(&saved).unwrap();
        let back: SavedDevice = serde_json::from_str(&json).unwrap();
        assert_eq!(back, saved);
        assert_eq!(back.confirm(&said()), Match::SameUnit);

        let old = serde_json::json!({ "info": table_info });
        let old: SavedDevice = serde_json::from_value(old).unwrap();
        assert!(old.learned().is_empty());
    }
}

/// A saved state is the value an app stores: the device it came from, the state's
/// id, and what the device gave (M2-111 §3.1.1).
mod saved_state {
    use tutti_midi_device::{
        MediaType, PeJson, PeState, SavedState, StateId, StateRev, UnixSeconds,
    };
    use tutti_ump::{DeviceIdentity, ManufacturerId, U7};

    fn device() -> DeviceIdentity {
        DeviceIdentity {
            manufacturer: ManufacturerId::of(0x0042),
            family: [1, 0].map(U7::of),
            model: [7, 0].map(U7::of),
            revision: [1, 2, 3, 4].map(U7::of),
        }
    }

    fn saved(state: PeState) -> SavedState {
        SavedState::new(device(), StateId::new("fullState").unwrap(), state)
    }

    fn round_trip(saved: &SavedState) -> SavedState {
        serde_json::from_str(&serde_json::to_string(saved).unwrap()).unwrap()
    }

    #[test]
    fn a_binary_state_reads_back_with_its_device_revision_and_time() {
        let state = PeState::octets(vec![0, 127, 128, 255])
            .with_rev(StateRev::new("buui890adj").unwrap())
            .with_timestamp(UnixSeconds::of(1_586_786_400));
        let saved = saved(state);
        assert_eq!(round_trip(&saved), saved);
    }

    #[test]
    fn a_json_state_reads_back_as_json() {
        let json = PeJson::parse(br#"{"a":[1,2]}"#).unwrap();
        let saved = saved(PeState::json(json));
        let back = round_trip(&saved);
        assert_eq!(back, saved);
        assert_eq!(back.state().to_wire().0, None);
    }

    #[test]
    fn the_document_names_each_part() {
        let state = PeState::binary(MediaType::new("audio/wav").unwrap(), vec![9])
            .unwrap()
            .with_timestamp(UnixSeconds::of(5));
        assert_eq!(
            serde_json::to_string(&saved(state)).unwrap(),
            r#"{"device":{"manufacturer":66,"family":[1,0],"model":[7,0],"revision":[1,2,3,4]},"state_id":"fullState","rev":null,"timestamp":5,"media_type":"audio/wav","bytes":[9]}"#
        );
    }

    #[test]
    fn a_document_that_breaks_a_rule_of_its_parts_is_refused() {
        let good = serde_json::to_string(&saved(PeState::octets(vec![1]))).unwrap();
        for (from, to) in [
            (r#""state_id":"fullState""#, r#""state_id":"no-way""#),
            (
                r#""media_type":"application/octet-stream""#,
                r#""media_type":"""#,
            ),
            (r#""timestamp":null"#, r#""timestamp":-1"#),
        ] {
            assert!(good.contains(from), "{good}");
            let bad = good.replace(from, to);
            assert!(serde_json::from_str::<SavedState>(&bad).is_err(), "{bad}");
        }
    }
}
