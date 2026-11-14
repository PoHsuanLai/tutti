use super::*;

/// The default host is always reachable — every supported platform has
/// one, and `available_hosts` must not return an empty list.
#[test]
fn the_default_host_is_always_available() {
    assert!(available_hosts().contains(&AudioHost::Default));
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
