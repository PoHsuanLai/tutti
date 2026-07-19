//! Thin helpers over raw AudioToolbox property and status APIs.

#![cfg(target_os = "macos")]

use std::mem::{size_of, MaybeUninit};
use std::os::raw::c_void;

use crate::error::{AuError, Result};
use crate::types::*;

/// Turn a raw `OSStatus` into a `Result`, attaching the calling function name
/// for diagnostics on failure.
pub(crate) fn check(function: &'static str, status: OSStatus) -> Result<()> {
    if status == NO_ERR {
        Ok(())
    } else {
        Err(AuError::OsStatus {
            function,
            code: status,
        })
    }
}

/// Fetch a `T`-valued AudioUnit property.
///
/// # Safety
/// The caller must know that the property at `(id, scope, element)` is exactly
/// `size_of::<T>()` bytes wide and that `T` is valid for any bit pattern the
/// AU might return (or is otherwise safely reinterpretable).
pub(crate) unsafe fn get_property<T>(
    unit: AudioUnit,
    id: u32,
    scope: u32,
    element: u32,
) -> Result<T> {
    let mut out: MaybeUninit<T> = MaybeUninit::uninit();
    let mut size = size_of::<T>() as u32;
    check(
        "AudioUnitGetProperty",
        AudioUnitGetProperty(
            unit,
            id,
            scope,
            element,
            out.as_mut_ptr() as *mut c_void,
            &mut size,
        ),
    )?;
    Ok(out.assume_init())
}

/// Write a `T`-valued AudioUnit property.
///
/// # Safety
/// The caller must ensure the property at `(id, scope, element)` accepts a
/// value of exactly `size_of::<T>()` bytes with the same in-memory layout as `T`.
pub(crate) unsafe fn set_property<T>(
    unit: AudioUnit,
    id: u32,
    scope: u32,
    element: u32,
    value: &T,
) -> Result<()> {
    check(
        "AudioUnitSetProperty",
        AudioUnitSetProperty(
            unit,
            id,
            scope,
            element,
            value as *const T as *const c_void,
            size_of::<T>() as u32,
        ),
    )
}

/// Query the byte size of an AudioUnit property without reading its value.
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub(crate) unsafe fn property_size(
    unit: AudioUnit,
    id: u32,
    scope: u32,
    element: u32,
) -> Result<u32> {
    let mut size: u32 = 0;
    let mut writable: i32 = 0;
    check(
        "AudioUnitGetPropertyInfo",
        AudioUnitGetPropertyInfo(unit, id, scope, element, &mut size, &mut writable),
    )?;
    Ok(size)
}

/// Read a variable-size property into an owned byte buffer.
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub(crate) unsafe fn get_property_bytes(
    unit: AudioUnit,
    id: u32,
    scope: u32,
    element: u32,
) -> Result<Vec<u8>> {
    let mut size = property_size(unit, id, scope, element)?;
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u8; size as usize];
    check(
        "AudioUnitGetProperty",
        AudioUnitGetProperty(
            unit,
            id,
            scope,
            element,
            buf.as_mut_ptr() as *mut c_void,
            &mut size,
        ),
    )?;
    buf.truncate(size as usize);
    Ok(buf)
}
