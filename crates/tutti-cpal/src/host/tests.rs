use super::*;

/// The default host is always reachable — every supported platform has
/// one, and `available_hosts` must not return an empty list.
#[test]
fn the_default_host_is_always_available() {
    assert!(available_hosts().contains(&AudioHost::Default));
}

/// **Asking for a host this build cannot serve is an error, not a silent
/// fallback.** Falling back to the default would send audio out of a
/// different device than the user chose, which is exactly the class of
/// failure this module exists to remove.
///
/// Mutation-checked: making `open_jack`'s unavailable arm return
/// `Ok(default_host())` fails this.
#[cfg(not(all(
    feature = "jack",
    any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd"
    )
)))]
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

/// A name selects the first device of that name; an id keeps its order.
#[test]
fn a_named_selector_is_the_first_device_of_that_name() {
    assert_eq!(
        DeviceSelector::named("USB"),
        DeviceSelector::Id(DeviceId::nth("USB", 0))
    );
    assert_ne!(
        DeviceSelector::named("USB"),
        DeviceSelector::Id(DeviceId::nth("USB", 1))
    );
}
