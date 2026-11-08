//! The plain-data types a property or parameter moves as bytes.

use std::os::raw::c_void;

use coreaudio_sys as sys;

mod sealed {
    pub trait Sealed {}
}
pub(crate) use sealed::Sealed;

/// A type every bit pattern of which is a valid value, and which the system
/// reads or writes as its in-memory bytes: the sized values an AudioUnit
/// property carries (`u32`, `f64`, `AudioStreamBasicDescription`, ...).
///
/// It is sealed. `Unit::get` starts from all-zero bytes and lets the unit
/// overwrite as many as it reports, so a `Plain` type must be valid when zero
/// and when filled with anything the unit may write; the impls below are the
/// integers, floats, raw pointers and `#[repr(C)]` structs of those that the
/// AudioToolbox headers define for the properties this crate reads.
///
/// # Safety
/// An implementor is `Copy`, has no invalid bit pattern (no `bool`, `char`,
/// reference or enum), and its all-zero value is valid.
pub unsafe trait Plain: Copy + sealed::Sealed {}

macro_rules! plain {
    ($($t:ty),* $(,)?) => {$(
        impl sealed::Sealed for $t {}
        // SAFETY: `$t` is an integer, a float, a raw pointer or a `#[repr(C)]`
        // struct of those (AudioToolbox's own plain-data records); each is
        // valid for every bit pattern, zero included.
        unsafe impl Plain for $t {}
    )*};
}

plain!(
    u8,
    u16,
    u32,
    i32,
    u64,
    f32,
    f64,
    *const c_void,
    sys::AudioStreamBasicDescription,
    sys::AudioUnitParameterInfo,
    sys::AudioUnitParameter,
    sys::AUPreset,
    sys::AUDependentParameter,
    sys::AURenderCallbackStruct,
    sys::AudioChannelLayout,
);
