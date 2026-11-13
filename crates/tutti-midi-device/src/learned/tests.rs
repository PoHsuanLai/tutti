use tutti_ump::{DeviceIdentity, ManufacturerId, U7};

use super::*;

fn identity(model: u8) -> DeviceIdentity {
    DeviceIdentity {
        manufacturer: ManufacturerId::of(0x0042),
        family: [1, 0].map(U7::of),
        model: [model, 0].map(U7::of),
        revision: [1, 2, 3, 4].map(U7::of),
    }
}

fn serial(text: &str) -> ProductInstanceId {
    ProductInstanceId::new(text).unwrap()
}

fn said(model: u8, id: Option<&str>) -> Learned {
    let learned = Learned::new().with_identity(identity(model));
    id.map_or(learned, |id| learned.with_instance_id(serial(id)))
}

/// M2-101 §5.8.3.1: the Product Instance Id is unique per unit, so two equal
/// ones are one unit.
#[test]
fn an_equal_product_instance_id_on_both_sides_is_the_same_unit() {
    assert_eq!(
        said(7, Some("A1")).compare(&said(7, Some("A1"))),
        Match::SameUnit
    );
    // One side never said its identity: the serial still names the unit.
    let only_serial = Learned::new().with_instance_id(serial("A1"));
    assert_eq!(said(7, Some("A1")).compare(&only_serial), Match::SameUnit);
}

/// Two units of one model share a Device Identity (M2-104 §7.1.3), so it
/// cannot name one: it is the model, and never the unit.
#[test]
fn an_identity_alone_is_never_the_same_unit() {
    assert_eq!(said(7, None).compare(&said(7, None)), Match::SameModel);
    assert_eq!(
        said(7, None).compare(&said(7, Some("B2"))),
        Match::SameModel
    );
    assert_eq!(
        said(7, Some("A1")).compare(&said(7, None)),
        Match::SameModel
    );
}

/// A field known on both sides and different refutes the saved device.
#[test]
fn a_field_that_differs_on_both_sides_is_a_different_device() {
    assert_eq!(said(7, None).compare(&said(8, None)), Match::Different);
    // The same model, another unit.
    assert_eq!(
        said(7, Some("A1")).compare(&said(7, Some("B2"))),
        Match::Different
    );
    // A serial that agrees does not outvote an identity that does not.
    assert_eq!(
        said(7, Some("A1")).compare(&said(8, Some("A1"))),
        Match::Different
    );
}

/// Nothing known on both sides for any field is no conclusion.
#[test]
fn nothing_comparable_is_unknown() {
    assert_eq!(Learned::new().compare(&said(7, Some("A1"))), Match::Unknown);
    assert_eq!(said(7, None).compare(&Learned::new()), Match::Unknown);
    // Each side knows a different field.
    let only_serial = Learned::new().with_instance_id(serial("A1"));
    assert_eq!(said(7, None).compare(&only_serial), Match::Unknown);
}

/// The source that declares the whole Endpoint wins; a peer fills the gaps.
#[test]
fn or_keeps_what_is_known_and_fills_the_rest() {
    let endpoint = said(7, None);
    let peer = said(9, Some("P1"));
    let merged = endpoint.or(peer);
    assert_eq!(merged.identity(), Some(&identity(7)));
    assert_eq!(merged.instance_id(), Some(&serial("P1")));
    assert!(Learned::new().or(Learned::new()).is_empty());
}

/// A saved device confirms through what it saved.
#[test]
fn a_saved_device_confirms_against_what_it_learned() {
    let info = DeviceInfo {
        id: crate::DeviceId::new(crate::Identity::alsa("K", "P"), 0),
        name: "K: P".into(),
        direction: crate::PortDirection::Input,
    };
    let saved = SavedDevice::new(info, said(7, Some("A1")));
    assert_eq!(saved.confirm(&said(7, Some("A1"))), Match::SameUnit);
    assert_eq!(saved.confirm(&said(8, None)), Match::Different);
}
