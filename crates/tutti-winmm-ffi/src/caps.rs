//! Listing the ports winmm has.

use windows_sys::Win32::Media::Audio::{
    MIDIINCAPSW, MIDIOUTCAPSW, midiInGetDevCapsW, midiInGetNumDevs, midiInMessage,
    midiOutGetDevCapsW, midiOutGetNumDevs, midiOutMessage,
};
use windows_sys::Win32::Media::Multimedia::{
    DRV_QUERYDEVICEINTERFACE, DRV_QUERYDEVICEINTERFACESIZE,
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

/// The longest interface path read, in bytes: a path past it is not one.
const MAX_INTERFACE_BYTES: u32 = 4096;

/// The device interface path of input port `index` (a `\\?\usb#vid_...`
/// string), which names the device across a replug where the list position
/// does not. `None` when the driver gives none.
#[must_use]
pub fn input_interface(index: u32) -> Option<String> {
    interface(|message, dw1, dw2| {
        // SAFETY: the port number stands for the handle in these two driver
        // messages; `dw1` and `dw2` are what `interface` says they are.
        unsafe {
            midiInMessage(
                std::ptr::without_provenance_mut(index as usize),
                message,
                dw1,
                dw2,
            )
        }
    })
}

/// The device interface path of output port `index`; see [`input_interface`].
#[must_use]
pub fn output_interface(index: u32) -> Option<String> {
    interface(|message, dw1, dw2| {
        // SAFETY: as `input_interface`.
        unsafe {
            midiOutMessage(
                std::ptr::without_provenance_mut(index as usize),
                message,
                dw1,
                dw2,
            )
        }
    })
}

/// Asks the driver, through `send(message, dw1, dw2)`, for the path's size in
/// bytes (`DRV_QUERYDEVICEINTERFACESIZE` writes a `u32` at `dw1`) and then for
/// the path (`DRV_QUERYDEVICEINTERFACE` writes a NUL-terminated UTF-16 string
/// into the buffer at `dw1` of `dw2` bytes). The pointers passed are live
/// buffers of those sizes for the call.
fn interface(send: impl Fn(u32, usize, usize) -> u32) -> Option<String> {
    let mut bytes = 0u32;
    if send(DRV_QUERYDEVICEINTERFACESIZE, (&raw mut bytes) as usize, 0) != 0
        || bytes == 0
        || bytes > MAX_INTERFACE_BYTES
    {
        return None;
    }
    let mut path = vec![0u16; (bytes as usize).div_ceil(2)];
    let room = path.len() * 2;
    if send(DRV_QUERYDEVICEINTERFACE, path.as_mut_ptr() as usize, room) != 0 {
        return None;
    }
    let path = name_of(&path);
    (!path.is_empty()).then_some(path)
}
