//! JACK, on a build that cannot reach it.

use super::{AudioHost, DeviceHost};
use crate::error::{Error, Result};

pub(super) fn open() -> Result<DeviceHost> {
    Err(Error::HostUnavailable {
        host: AudioHost::Jack,
        reason: "built without the `jack` feature, or not a JACK platform".into(),
    })
}

/// **Asking for a host this build cannot serve is an error, not a silent
/// fallback.** Falling back to the default would send audio out of a
/// different device than the user chose, which is exactly the class of
/// failure this module exists to remove.
#[test]
fn an_unreachable_host_is_refused_rather_than_substituted() {
    let err = DeviceHost::open(AudioHost::Jack).expect_err("JACK is not reachable in this build");
    assert!(
        matches!(
            err,
            Error::HostUnavailable {
                host: AudioHost::Jack,
                ..
            }
        ),
        "expected HostUnavailable, got {err:?}"
    );
}
