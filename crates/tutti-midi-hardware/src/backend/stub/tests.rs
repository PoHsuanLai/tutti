use super::*;

#[test]
fn it_enumerates_nothing() {
    let b = StubEndpoints::new();
    assert!(b.inputs().is_empty());
    assert!(b.outputs().is_empty());
}

/// Opening must be `Unsupported`, not a not-found error — the caller renders
/// these differently, and conflating them hides a build fact behind a
/// hardware one.
#[test]
fn opening_reports_unsupported_rather_than_not_found() {
    let b = StubEndpoints::new();
    assert!(matches!(
        b.open_output(EndpointId::from_raw(0)),
        Err(Error::Unsupported(_))
    ));
}
