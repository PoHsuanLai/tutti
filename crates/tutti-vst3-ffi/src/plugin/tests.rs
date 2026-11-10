use super::*;
use vst3::Steinberg::kResultOk;

/// A string longer than the field is cut to 63 bytes and still terminated;
/// a short one is copied whole.
///
/// Mutation: write `dst.len()` bytes (drop the `- 1`) and the long case
/// overruns the terminator, so the last byte is not 0.
#[test]
fn fill_char8_64_truncates_and_terminates() {
    let mut dst = [1i8; 64];
    fill_char8_64(&mut dst, &"x".repeat(100));
    assert!(dst[..63].iter().all(|&b| b == b'x' as i8));
    assert_eq!(dst[63], 0);

    let mut dst = [1i8; 64];
    fill_char8_64(&mut dst, "abc");
    assert_eq!(&dst[..4], &[b'a' as i8, b'b' as i8, b'c' as i8, 0]);
}

/// `kNotImplemented` is a successful state call, a refusal is not.
///
/// Mutation: drop the `NotImplemented` arm and the first assert fails.
#[test]
fn a_state_call_that_is_not_implemented_is_not_a_failure() {
    assert_eq!(state_ok(vst3::Steinberg::kNotImplemented), Ok(()));
    assert_eq!(state_ok(kResultOk), Ok(()));
    assert_eq!(
        state_ok(vst3::Steinberg::kInternalError),
        Err(Vst3Error::InternalError)
    );
}

/// The raw ABI values of the bus enums are the SDK's.
///
/// Mutation: swap the two arms of `BusDirection::raw` and the pinned
/// values fail.
#[test]
fn the_bus_enums_carry_the_sdk_ordinals() {
    assert_eq!(MediaType::Audio.raw(), 0);
    assert_eq!(MediaType::Event.raw(), 1);
    assert_eq!(BusDirection::Input.raw(), 0);
    assert_eq!(BusDirection::Output.raw(), 1);
}

/// The compile-time threading rules: a `Plugin` moves between threads and
/// is not shared.
///
/// Mutation: add `unsafe impl Sync for Plugin {}` and the probe below stops
/// compiling: both impls of `AmbiguousIfSync` then apply to `Plugin`, so
/// naming the trait with an inferred parameter is ambiguous.
#[test]
fn a_plugin_is_send_and_not_sync() {
    fn is_send<T: Send>() {}
    is_send::<Plugin>();

    trait AmbiguousIfSync<A> {
        fn probe() {}
    }
    impl<T: ?Sized> AmbiguousIfSync<()> for T {}
    impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}
    <Plugin as AmbiguousIfSync<_>>::probe();
}

/// **A media type or direction the SDK does not define names none**, and each
/// defined one round-trips.
///
/// Mutation (run, fails): make `from_raw` fall back to `Some(Self::Audio)` /
/// `Some(Self::Input)` for an unknown value -> the `99` assertions fail.
#[test]
fn an_undefined_media_type_or_direction_is_none() {
    assert_eq!(MediaType::from_raw(99), None);
    assert_eq!(BusDirection::from_raw(99), None);
    for kind in [MediaType::Audio, MediaType::Event] {
        assert_eq!(MediaType::from_raw(kind.raw()), Some(kind));
    }
    for direction in [BusDirection::Input, BusDirection::Output] {
        assert_eq!(BusDirection::from_raw(direction.raw()), Some(direction));
    }
}
