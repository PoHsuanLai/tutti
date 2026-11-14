//! Listing the ports winmm has.

use windows_sys::Win32::Media::Audio::{
    MIDIINCAPSW, MIDIOUTCAPSW, midiInGetDevCapsW, midiInGetNumDevs, midiOutGetDevCapsW,
    midiOutGetNumDevs,
};

use crate::error::{Error, check};

/// What a driver reports of one port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caps {
    /// The manufacturer id (`wMid`).
    pub manufacturer: u16,
    /// The product id (`wPid`).
    pub product: u16,
    /// The product name (`szPname`), which winmm cuts to 31 characters.
    pub name: String,
}

/// How many input ports winmm lists now.
#[must_use]
pub fn input_count() -> u32 {
    // SAFETY: takes no arguments and touches no memory of ours.
    unsafe { midiInGetNumDevs() }
}

/// How many output ports winmm lists now.
#[must_use]
pub fn output_count() -> u32 {
    // SAFETY: takes no arguments and touches no memory of ours.
    unsafe { midiOutGetNumDevs() }
}

/// What the driver reports of input port `index`.
///
/// # Errors
///
/// `MMSYSERR_BADDEVICEID` when the port is gone, or the driver's own refusal.
pub fn input_caps(index: u32) -> Result<Caps, Error> {
    let mut caps = MIDIINCAPSW::default();
    // SAFETY: `caps` is a live, writable `MIDIINCAPSW` and the size passed is
    // its size, so winmm writes within it.
    let code = unsafe {
        midiInGetDevCapsW(
            index as usize,
            &raw mut caps,
            size_of::<MIDIINCAPSW>() as u32,
        )
    };
    check("midiInGetDevCaps", code)?;
    // The struct is packed: read each field by value, never by reference.
    let (manufacturer, product, name) = (caps.wMid, caps.wPid, caps.szPname);
    Ok(Caps {
        manufacturer,
        product,
        name: name_of(&name),
    })
}

/// What the driver reports of output port `index`.
///
/// # Errors
///
/// `MMSYSERR_BADDEVICEID` when the port is gone, or the driver's own refusal.
pub fn output_caps(index: u32) -> Result<Caps, Error> {
    let mut caps = MIDIOUTCAPSW::default();
    // SAFETY: `caps` is a live, writable `MIDIOUTCAPSW` and the size passed is
    // its size, so winmm writes within it.
    let code = unsafe {
        midiOutGetDevCapsW(
            index as usize,
            &raw mut caps,
            size_of::<MIDIOUTCAPSW>() as u32,
        )
    };
    check("midiOutGetDevCaps", code)?;
    let (manufacturer, product, name) = (caps.wMid, caps.wPid, caps.szPname);
    Ok(Caps {
        manufacturer,
        product,
        name: name_of(&name),
    })
}

/// The text of a NUL-terminated UTF-16 field.
fn name_of(field: &[u16]) -> String {
    let end = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    String::from_utf16_lossy(&field[..end])
}
