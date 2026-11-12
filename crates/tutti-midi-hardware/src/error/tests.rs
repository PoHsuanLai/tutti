use super::*;
use std::error::Error as _;

/// **An error made from another carries it as its source**, so a caller that
/// walks `source()` reaches the OS error, and one made without a cause has
/// none.
#[test]
fn a_device_or_port_error_keeps_the_error_under_it() {
    let io = std::io::Error::other("no threads left");
    let error = Error::device_from("spawn MIDI pump", io);
    assert_eq!(error.to_string(), "MIDI device error: spawn MIDI pump");
    assert_eq!(
        error.source().map(ToString::to_string).as_deref(),
        Some("no threads left")
    );
    let error = Error::port_from("watch", std::io::Error::other("gone"));
    assert!(error.source().is_some());
    assert!(Error::device("plain").source().is_none());
    assert!(Error::port("plain").source().is_none());
}

/// **`NoDeviceNamed` says what was searched**, in its message too.
#[test]
fn a_name_that_matched_nothing_lists_its_candidates() {
    let error = Error::NoDeviceNamed {
        name: "keystpe".into(),
        searched: vec!["Keystep Pro".into(), "IAC Bus 1".into()],
    };
    assert_eq!(
        error.to_string(),
        "no MIDI device named like 'keystpe' (searched: Keystep Pro, IAC Bus 1)"
    );
}
